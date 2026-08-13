//! Reversibility COVERAGE: every advertised verb has been looked at by a human.
//!
//! `zbus::rev` defaults an unknown verb to `irreversible`, which is the safe answer but an
//! indistinguishable one — a verb nobody has classified yet and a verb deliberately ruled
//! un-undoable both come back `"irreversible"`. The consequence is silent: a verb added to
//! `SURFACE_VERBS` costs every transaction that touches it, and nothing fails to say so.
//!
//! This file removes that ambiguity. [`DELIBERATELY_IRREVERSIBLE`] is the ledger of verbs examined
//! and ruled un-undoable, each with the reason. Together with `zbus`'s `REV` table it must cover the
//! surface EXACTLY: a new verb fails [`every_surface_verb_is_classified`] until someone either
//! classifies it in `REV` or writes down here why it cannot be. The classification is a snapshot of
//! what the code does today; this test is what keeps the snapshot honest.
//!
//! The ledger is a test fixture on purpose. `REV` is served to clients (`surface()`), and the HUD
//! trigger editor reads it — a second runtime table would be a second thing to keep in sync. What
//! belongs here is the review record, and the review record is not wire state.
//!
//! Reasons are grouped, not repeated per line. Two rules decide every entry:
//!
//! * A HOST verb can never be `inverse`: `txn.rs` journals a step's verb and args, never its
//!   pre-state, so nothing exists to restore from. It is `pure` only if it has no persistent effect
//!   at all — no write, no spawn, no publish, no OS-visible artifact.
//! * A `browser.*` verb is `inverse` only when the HUD journal OBSERVES all of its effects
//!   (`zjournal.js`: tab created / removed / moved / detached / pinned / muted / url / activated /
//!   zoomed, window created) and can REPLAY them (reopen, close, closeWindow, move, flags, activate,
//!   navigate, zoom). Anything else journals nothing, and an abort would report a clean revert
//!   having restored nothing.

use std::collections::BTreeSet;

use zwire_host::zbus;

/// Verbs examined and ruled un-undoable, with the reason each cannot be promoted.
///
/// An entry leaves this list only when the verb is classified in `REV` — the two lists are
/// disjoint, and [`the_ledger_only_lists_verbs_that_are_actually_irreversible`] enforces it.
const DELIBERATELY_IRREVERSIBLE: &[&str] = &[
    /* ---- host: writes with no captured pre-state ---------------------------------------------
    `fs_write` / `fs_append` overwrite or extend a file and additionally `create_dir_all` the
    parent (fsops.rs `write`); nothing reads the prior bytes first, so there is nothing to put
    back. `fs_rm` deletes (recursively, with `recursive`). `fs_mkdir` is `create_dir_all`, which
    succeeds silently when the directory already exists and creates any number of intermediate
    levels when it does not — `fs_rm` as its "inverse" would happily delete a directory tree the
    call never created. `clipboard_set` pipes to pbcopy/wl-copy/clip (osops.rs) over whatever was
    on the clipboard; `clipboard_get` exists but nobody calls it first, so the prior contents are
    simply gone. The kv writes are the same story against the file-backed store. */
    "clipboard_set",
    "fs_append",
    "fs_mkdir",
    "fs_rm",
    "fs_write",
    "kv_del",
    "kv_merge",
    "kv_set",
    /* ---- host: the file browser's mutating ops (fsx.rs) ---------------------------------------
    Every verb below changes the filesystem, and `txn.rs` journals the verb and its args — never
    the bytes, mode, timestamps or link target that were there first. So none of them can be
    replayed backwards, even where an inverse verb exists in the surface.

    Destroying: `fs_delete_file` removes a file or a whole tree; `fs_secure_delete` first
    OVERWRITES the file's bytes with zeros and fsyncs before unlinking, so it is irreversible in
    the strongest sense available — the prior contents no longer exist on the device.
    `fs_move_to_trash` is recoverable by the USER from the OS trash, but not by this process: the
    trash entry's identifier is never captured, so an abort has nothing to address.

    Creating: `fs_create_dir`, `fs_create_file`, `fs_copy_path`, `fs_duplicate`, `fs_compress` and
    `fs_extract` all leave new paths on disk. Deleting them as a compensation would be a guess —
    `fs_extract` in particular writes an unbounded set of entries, and only the archive knows
    which. `fs_touch` additionally CREATES the file when it is absent, so even its "just a
    timestamp" case is not uniformly a metadata-only change.

    Rewriting in place: `fs_rename_file` moves a path (and silently replaces an existing
    destination, per `std::fs::rename`); `fs_chmod` replaces the permission bits without reading
    the old ones; `fs_symlink_retarget` UNLINKS the symlink and recreates it, so the previous
    target is gone the moment the first step succeeds; `fs_touch` overwrites atime and mtime. */
    "fs_chmod",
    "fs_compress",
    "fs_copy_path",
    "fs_create_dir",
    "fs_create_file",
    "fs_delete_file",
    "fs_duplicate",
    "fs_extract",
    "fs_move_to_trash",
    "fs_rename_file",
    "fs_secure_delete",
    "fs_symlink_retarget",
    "fs_touch",
    /* ---- host: runs a program ----------------------------------------------------------------
    Whatever the child did is outside this process and outside any journal. `hooks_test_run` is
    documented as a dry run only in the sense that it does not DISPATCH the parsed actions — it
    still executes the hook's stryke script. `hook_fire` runs every enabled hook for the event AND
    dispatches what they print. */
    "exec",
    "hook_fire",
    "hooks_test_run",
    "job_start",
    "kill",
    "open",
    "stryke_run",
    /* ---- host: acts inside ANOTHER process ----------------------------------------------------
    `suite_call` invokes a verb on a different app over its bus socket (suite.rs). Whatever that
    verb did happened in that app's address space and its own transaction journal, neither of which
    this host can read or replay — `txn.rs` journals a verb and its args, so an "inverse" here would
    have to guess the peer's opposite verb and its arguments. Cross-app rollback is a real thing and
    it already has an owner: zgo's saga coordinator enlists each participant under its OWN
    transaction and fans `abort` back out, so each app compensates through the inverse it declared.
    A chain that needs all-or-nothing across apps calls `saga.run` THROUGH this verb rather than
    asking zwire to invent a second coordinator. (`suite_list` / `suite_verbs` / `suite_get` are
    reads and are classed `pure` in `REV`.) */
    "suite_call",
    /* ---- host: mutates stored automation ------------------------------------------------------
    hooks.rs rewrites `hooks.json` and the per-hook `<id>.st` script in place. `hooks_delete` also
    unlinks the script file. No prior manifest or script text is captured anywhere. */
    "hooks_delete",
    "hooks_save",
    "hooks_set_enabled",
    "hooks_set_script",
    /* ---- host: spawns a thread, a child process, or a stream ----------------------------------
    `fs_watch` / `fs_tail` / `meter_stream` each start a background poll thread (watch.rs
    `Watcher::spawn`) that keeps writing frames to the connection — `fs_tail` reads a file, but a
    verb that leaves a thread running is not a read. `sysinfo_start` starts the sysmon thread,
    `pty_spawn` a shell, `stryke_lsp_start` a language server. The `*_stop` verbs and `pty_kill`
    tear those down, destroying the stream position, the PTY's scrollback and the child's state;
    restarting is not restoring. `pty_write` feeds a live shell, `pty_resize` changes its geometry
    with no record of the old one, `stryke_lsp_send` writes to a server's stdin. */
    "fs_tail",
    "fs_watch",
    "meter_stream",
    "pty_kill",
    "pty_resize",
    "pty_spawn",
    "pty_write",
    "stryke_lsp_send",
    "stryke_lsp_start",
    "stryke_lsp_stop",
    "sysinfo_start",
    "sysinfo_stop",
    "watch_stop",
    /* ---- host: sends something that cannot be recalled ----------------------------------------
    `pub` fans a message to every local subscriber and forwards it to every peer host; `sub` /
    `unsub` mutate that subscriber set (and `sub` immediately pushes a snapshot frame);
    `peer_connect` dials another host and joins a mesh; `notify` puts a desktop notification on the
    user's screen via osascript / notify-send. */
    "notify",
    "peer_connect",
    "pub",
    "sub",
    "unsub",
    /* ---- browser: effects the HUD journal cannot see ------------------------------------------
    Window state, bounds and focus produce no event this journal listens to — it hooks
    `windows.onCreated` and nothing else on windows. `snap*` / `centerWindow` /
    `moveWindowNextDisplay` rewrite left/top/width/height; the minimize / maximize / fullscreen /
    restore group rewrites `state`; `nextWindow` / `prevWindow` change focus. `restoreWindow` looks
    like the inverse of minimize, but nothing records which state the window was in, so it is a
    guess, not a compensation. */
    "browser.centerWindow",
    "browser.fullscreenWindow",
    "browser.maximizeWindow",
    "browser.minimizeWindow",
    "browser.moveWindowNextDisplay",
    "browser.nextWindow",
    "browser.prevWindow",
    "browser.restoreWindow",
    "browser.snapBottom",
    "browser.snapBottomLeft",
    "browser.snapBottomRight",
    "browser.snapLeft",
    "browser.snapRight",
    "browser.snapTop",
    "browser.snapTopLeft",
    "browser.snapTopRight",
    /* ---- browser: destroys the container the undo would restore into --------------------------
    `closeWindow` removes a window; the journal has no `windows.onRemoved` hook, and the per-tab
    `reopen` steps its tabs do produce name a `windowId` that no longer exists, so the replay would
    fail tab by tab. `mergeWindows` moves every tab into the active window, which EMPTIES and
    therefore destroys the source windows — the recorded `move` back to the old `windowId` has
    nowhere to land. */
    "browser.closeWindow",
    "browser.mergeWindows",
    /* ---- browser: not observable, or not exactly restorable -----------------------------------
    `reopenTab` calls `sessions.restore()`, which consumes an entry from the recently-closed store
    and may restore a tab OR a whole window; closing what it restored puts a NEW entry back with a
    different sessionId, so the round trip is not exact on the store a script can query.
    `discardTab` unloads the tab, discarding in-page state (and Chrome may replace the tab id);
    there is no "un-discard" op. `incognitoWindow` creates a window the extension cannot see at all
    unless it has been granted incognito access, so `windows.onCreated` may never fire and the
    abort would compensate nothing while reporting success. Bulk PIN reorders the strip — pinned
    tabs are forced into the pinned region — so the replay depends on whether the `flags` op or the
    `move` op runs first for a given tab; order-dependent is not inverse. */
    "browser.discardTab",
    "browser.incognitoWindow",
    "browser.pinAll",
    "browser.reopenTab",
    "browser.unpinAll",
    /* ---- browser: writes an artifact outside the tab strip ------------------------------------
    Previously classed `pure`, and both were wrong. `screenshot` captures the visible tab and then
    `downloads.download`s it — a PNG on disk plus a downloads-history entry. `detectLanguage` reads
    the language and then raises a desktop notification. A `pure` verb runs inside a transaction
    unjournaled and uncompensated, which for these two means an aborted chain leaves the file and
    the notification behind. */
    "browser.detectLanguage",
    "browser.screenshot",
    /* ---- browser: tab groups ------------------------------------------------------------------
    The journal has no `tabGroups` hooks. Worse than merely uncompensated: grouping MOVES tabs to
    make the group contiguous, so a journaled run would record `move` ops and an abort would shuffle
    the strip back while leaving every tab still grouped — a partial revert reported as a clean one.
    `ungroupTabs` does not restore a group's title or color either. */
    "browser.collapseGroups",
    "browser.expandGroups",
    "browser.groupTabs",
    "browser.ungroupTabs",
    /* ---- browser: downloads --------------------------------------------------------------------
    `download` and `retryDownload` fetch bytes to disk; `cancelDownload` discards a partial file;
    `clearDownloads` erases history records. `pause` / `resume` read as a pair but each targets "the
    most recent matching item", not a captured id, and a paused transfer is not guaranteed to
    resume. `openDownload` launches the file in its external handler and `showDownload` /
    `showDownloads` open the OS file manager. */
    "browser.cancelDownload",
    "browser.clearDownloads",
    "browser.download",
    "browser.openDownload",
    "browser.pauseDownload",
    "browser.resumeDownload",
    "browser.retryDownload",
    "browser.showDownload",
    "browser.showDownloads",
    /* ---- browser: deletes user data -----------------------------------------------------------
    All-time deletions through `browsingData` / `history`. Nothing restores a cache, a cookie jar,
    or a history range. (`clearPasswords` currently reaches no branch of the executor — Chromium
    maps `removePasswords` to an empty removal mask — but a verb that is a no-op only by accident
    of the current implementation is not `pure`; classing it so would turn a future implementation
    into a silent data-loss path.) */
    "browser.clearAllData",
    "browser.clearCache",
    "browser.clearCacheAndCookies",
    "browser.clearCookies",
    "browser.clearHistory",
    "browser.clearPasswords",
    "browser.deleteHistoryUrl",
    /* ---- browser: bookmarks, reading list, history additions ----------------------------------
    Each of these has a named opposite verb on the surface, and every one of them is a trap. The
    journal records nothing for bookmarks, the reading list or history, so an abort compensates
    nothing whatever this table claims. And the opposites are not exact even by hand:
    `removeBookmark` deletes EVERY bookmark matching the tab's url (including one that predates the
    chain), `removeReadingList` removes the entry whether or not this call added it, and
    `deleteHistoryUrl` drops all visits to a url, not the one just added. */
    "browser.addHistoryUrl",
    "browser.addReadingList",
    "browser.bookmarkFolder",
    "browser.bookmarkTab",
    "browser.removeBookmark",
    "browser.removeReadingList",
    /* ---- browser: machine + extension state ---------------------------------------------------
    `keepAwake` / `keepDisplayAwake` hold a power lock and `allowSleep` releases it, with no record
    of which level (if any) was held before. `enableExtension` / `disableExtension` flip another
    extension's state without capturing it, `uninstallExtension` removes it outright, `launchApp`
    starts one. `notify` raises a notification that cannot be unsent. `tmux` drives a terminal
    session through the HUD. */
    "browser.allowSleep",
    "browser.disableExtension",
    "browser.enableExtension",
    "browser.keepAwake",
    "browser.keepDisplayAwake",
    "browser.launchApp",
    "browser.notify",
    "browser.tmux",
    "browser.uninstallExtension",
];

/// Every verb on the advertised surface, as the surface itself reports it.
fn surface_verbs() -> Vec<String> {
    zbus::surface()["verbs"]
        .as_array()
        .expect("the surface advertises a verb array")
        .iter()
        .map(|v| {
            v["id"]
                .as_str()
                .expect("every advertised verb has an id")
                .to_string()
        })
        .collect()
}

/// THE GATE. Every advertised verb is either classified in `REV` or written down in the ledger with
/// a reason. A verb added to `SURFACE_VERBS` and left alone defaults to `irreversible` at runtime —
/// safe, but it quietly costs every transaction that touches it, and without this test nothing says
/// so. Failing here is the prompt to make the call deliberately.
#[test]
fn every_surface_verb_is_classified() {
    let ledger: BTreeSet<&str> = DELIBERATELY_IRREVERSIBLE.iter().copied().collect();
    let unreviewed: Vec<String> = surface_verbs()
        .into_iter()
        .filter(|id| zbus::rev(id) == "irreversible" && !ledger.contains(id.as_str()))
        .collect();
    assert!(
        unreviewed.is_empty(),
        "{} verb(s) are neither classified in zbus::REV nor listed in DELIBERATELY_IRREVERSIBLE: \
         {unreviewed:?}. Read the implementation, then either add it to REV as `pure`/`inverse` or \
         add it to the ledger with the reason it cannot be undone.",
        unreviewed.len()
    );
}

/// The ledger may only hold verbs that really are irreversible. Promoting a verb to `pure` or
/// `inverse` without deleting its ledger entry would leave a written reason contradicting the table
/// that is actually served — the next reader could not tell which one is the review.
#[test]
fn the_ledger_only_lists_verbs_that_are_actually_irreversible() {
    let promoted: Vec<(&str, &str)> = DELIBERATELY_IRREVERSIBLE
        .iter()
        .map(|id| (*id, zbus::rev(id)))
        .filter(|(_, class)| *class != "irreversible")
        .collect();
    assert!(
        promoted.is_empty(),
        "the ledger still lists verb(s) that REV now classifies: {promoted:?} — delete the ledger \
         entry when you promote a verb"
    );
}

/// A stale ledger entry — a verb renamed or dropped from the surface — is a reason recorded about
/// something that no longer exists, and it hides the rename from the coverage gate above (the new
/// name shows up as unreviewed, but the old one silently keeps passing).
#[test]
fn the_ledger_has_no_entries_that_left_the_surface() {
    let surface: BTreeSet<String> = surface_verbs().into_iter().collect();
    let stale: Vec<&&str> = DELIBERATELY_IRREVERSIBLE
        .iter()
        .filter(|id| !surface.contains(**id))
        .collect();
    assert!(
        stale.is_empty(),
        "the ledger names verb(s) that are not on the surface: {stale:?}"
    );
}

/// A duplicated ledger entry is two reviews of one verb, which can disagree. Cheap to forbid.
#[test]
fn the_ledger_lists_each_verb_once() {
    let mut seen = BTreeSet::new();
    let dupes: Vec<&&str> = DELIBERATELY_IRREVERSIBLE
        .iter()
        .filter(|id| !seen.insert(**id))
        .collect();
    assert!(dupes.is_empty(), "duplicate ledger entries: {dupes:?}");
}

/// The classifications this round promoted, pinned individually. The coverage gate above only
/// proves each verb was LOOKED at; these assertions pin the verdict, so a later edit that flips one
/// of them — turning a compensated step into an unjournaled one, or the reverse — has to be
/// deliberate. Each is `inverse` because the HUD journal observes and replays every effect it
/// produces: a navigate, a per-tab mute flag, a set of tab moves, or one created tab.
#[test]
fn the_promoted_verbs_keep_their_class() {
    for verb in [
        "browser.home",
        "browser.muteAll",
        "browser.unmuteAll",
        "browser.sortTabs",
        "browser.extensionOptions",
    ] {
        assert_eq!(zbus::rev(verb), "inverse", "{verb}");
    }
    // Both were classed `pure` and both leave an artifact behind — a downloaded PNG and a desktop
    // notification. `pure` means "runs inside a transaction, journals nothing, compensates
    // nothing", which is exactly the wrong promise for either.
    for verb in ["browser.screenshot", "browser.detectLanguage"] {
        assert_eq!(zbus::rev(verb), "irreversible", "{verb}");
    }
}

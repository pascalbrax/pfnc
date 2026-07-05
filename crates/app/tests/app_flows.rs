//! End-to-end tests driving `App::handle_key`/`handle_job_event` with
//! synthetic key events against a real temp directory tree (via the real
//! `LocalFs` backend) — no terminal involved.

use std::fs;
use std::time::{Duration, Instant};

use camino::Utf8PathBuf;
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use tempfile::tempdir;

use pfnc::actions::{handle_job_event, handle_key};
use pfnc::app::App;
use pfnc_config::{Config, PanelLocationConfig, PanelSessionConfig};
use pfnc_core::{JobEvent, JobId, JobKind, JobOutcome, Location, Mode, ProgressState, SavedProfileSummary, VfsPath};

fn vfs_path(p: &std::path::Path) -> VfsPath {
    Utf8PathBuf::from_path_buf(p.to_path_buf()).expect("tempdir path must be UTF-8")
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn press(app: &mut App, code: KeyCode) {
    let mut ev = key(code);
    ev.kind = KeyEventKind::Press;
    handle_key(app, ev);
}

/// Drains job events until the currently-running job (if any) finishes,
/// applying each event the same way the real main loop would. Times out
/// rather than hanging forever if something is wrong.
fn wait_for_job_to_finish(app: &mut App) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while matches!(app.mode, Mode::Progress(_)) {
        assert!(Instant::now() < deadline, "job did not finish within timeout");
        if let Ok(event) = app.jobs.events().recv_timeout(Duration::from_millis(100)) {
            handle_job_event(app, event);
        }
    }
}

#[test]
fn f10_quits_like_q_and_esc() {
    let dir = tempdir().unwrap();
    let mut app = App::new(vfs_path(dir.path()));
    assert!(!app.should_quit);
    press(&mut app, KeyCode::F(10));
    assert!(app.should_quit);
}

#[test]
fn f1_opens_help_and_any_key_dismisses_it() {
    let dir = tempdir().unwrap();
    let mut app = App::new(vfs_path(dir.path()));

    press(&mut app, KeyCode::F(1));
    assert!(matches!(app.mode, Mode::Help));

    press(&mut app, KeyCode::Char('x'));
    assert!(matches!(app.mode, Mode::Browsing), "any key should dismiss the help box");
}

#[test]
fn session_restore_uses_saved_local_cwd_when_it_still_exists() {
    let dir = tempdir().unwrap();
    let saved_dir = dir.path().join("saved");
    fs::create_dir(&saved_dir).unwrap();

    let mut config = Config::default();
    config.session.left = Some(PanelSessionConfig {
        location: PanelLocationConfig::Local,
        cwd: saved_dir.to_str().unwrap().to_string(),
    });
    // Right panel has no saved session, so it should fall back.

    let fallback = vfs_path(dir.path());
    let app = App::new_with_config(config, fallback.clone());

    assert_eq!(app.left.cwd, vfs_path(&saved_dir));
    assert_eq!(app.right.cwd, fallback);
}

#[test]
fn session_restore_falls_back_when_saved_dir_no_longer_exists() {
    let dir = tempdir().unwrap();
    let mut config = Config::default();
    config.session.left = Some(PanelSessionConfig {
        location: PanelLocationConfig::Local,
        cwd: dir.path().join("deleted").to_str().unwrap().to_string(),
    });

    let fallback = vfs_path(dir.path());
    let app = App::new_with_config(config, fallback.clone());

    assert_eq!(app.left.cwd, fallback);
}

#[test]
fn sync_session_into_config_only_persists_local_panels() {
    let dir = tempdir().unwrap();
    let mut app = App::new(vfs_path(dir.path()));
    app.right.location = Location::Remote {
        profile_id: "u@h:22".to_string(),
    };

    app.sync_session_into_config();

    assert!(app.config.session.left.is_some());
    assert!(app.config.session.right.is_none());
}

#[test]
fn successful_connect_upserts_profile_in_memory() {
    let dir = tempdir().unwrap();
    let mut app = App::new(vfs_path(dir.path()));
    assert!(app.config.profiles.is_empty());

    let job_id: JobId = 42;
    app.mode = Mode::Progress(ProgressState {
        job_id,
        title: "Connecting".into(),
        progress: Default::default(),
        kind: JobKind::Connect {
            target_left: true,
            profile: SavedProfileSummary {
                id: "alice@example.com:22".to_string(),
                host: "example.com".to_string(),
                port: 22,
                username: "alice".to_string(),
            },
        },
    });

    // Drive the pure completion logic directly rather than performing a
    // real network connect — this test is about the profile-save/panel
    // update bookkeeping, not SFTP itself (covered in pfnc-vfs-sftp).
    handle_job_event(
        &mut app,
        JobEvent::Finished {
            job_id,
            outcome: JobOutcome::Completed,
        },
    );

    assert_eq!(app.config.profiles.len(), 1);
    assert_eq!(app.config.profiles[0].host, "example.com");
    assert_eq!(
        app.left.location,
        Location::Remote {
            profile_id: "alice@example.com:22".to_string()
        }
    );
}

fn make_test_tar(dir: &std::path::Path) {
    let tar_path = dir.join("backup.tar");
    let file = fs::File::create(&tar_path).unwrap();
    let mut builder = tar::Builder::new(file);
    let mut header = tar::Header::new_gnu();
    let data = b"archived content";
    header.set_size(data.len() as u64);
    header.set_mode(0o644);
    header.set_cksum();
    builder.append_data(&mut header, "inner/note.txt", &data[..]).unwrap();
    builder.finish().unwrap();
}

#[test]
fn enter_archive_browse_and_pop_back_out() {
    use pfnc_core::Location;

    let dir = tempdir().unwrap();
    let root = vfs_path(dir.path());
    make_test_tar(dir.path());

    let mut app = App::new(root.clone());
    // Only entry in the root is "backup.tar" (cursor already on it).
    press(&mut app, KeyCode::Enter);
    wait_for_job_to_finish(&mut app);

    assert!(matches!(app.left.location, Location::Archive { .. }));
    assert_eq!(app.left.cwd, VfsPath::from("/"));
    assert_eq!(app.left.entries.len(), 1);
    assert_eq!(app.left.entries[0].name, "inner");

    // Descend into the archive's own subdirectory.
    press(&mut app, KeyCode::Enter);
    assert_eq!(app.left.cwd, VfsPath::from("/inner"));
    assert_eq!(app.left.entries[0].name, "note.txt");

    // Backspace out of the subdirectory, back to the archive root.
    press(&mut app, KeyCode::Backspace);
    assert_eq!(app.left.cwd, VfsPath::from("/"));
    assert!(matches!(app.left.location, Location::Archive { .. }));

    // Backspace again at the archive root pops all the way back out to
    // the real filesystem location the archive file lives in.
    press(&mut app, KeyCode::Backspace);
    assert_eq!(app.left.location, Location::Local);
    assert_eq!(app.left.cwd, root);
    assert!(app.left.entries.iter().any(|e| e.name == "backup.tar"));
}

#[test]
fn extract_file_out_of_archive_via_copy() {
    let dir = tempdir().unwrap();
    let root = vfs_path(dir.path());
    make_test_tar(dir.path());
    fs::create_dir(dir.path().join("dest")).unwrap();

    let mut app = App::new(root.clone());
    // Entries sorted folders-first: dest(0), backup.tar(1).
    press(&mut app, KeyCode::Down); // onto backup.tar
    press(&mut app, KeyCode::Enter); // open backup.tar
    wait_for_job_to_finish(&mut app);
    press(&mut app, KeyCode::Enter); // descend into inner/

    // Right panel -> dest (cursor already on it, folders sort first).
    press(&mut app, KeyCode::Tab);
    press(&mut app, KeyCode::Enter);
    press(&mut app, KeyCode::Tab); // back to the archive panel

    press(&mut app, KeyCode::F(5));
    press(&mut app, KeyCode::Enter); // confirm prefilled "dest" destination
    wait_for_job_to_finish(&mut app);

    assert_eq!(fs::read(root.join("dest/note.txt")).unwrap(), b"archived content");
}

#[test]
fn cursor_resets_to_top_when_entering_a_directory() {
    let dir = tempdir().unwrap();
    let root = vfs_path(dir.path());
    fs::create_dir(dir.path().join("a")).unwrap();
    fs::create_dir(dir.path().join("b")).unwrap();
    fs::write(dir.path().join("b/inner.txt"), b"x").unwrap();

    let mut app = App::new(root);
    // Move cursor down to "b" (entries sorted: a, b), then descend.
    press(&mut app, KeyCode::Down);
    assert_eq!(app.left.cursor, 1);
    press(&mut app, KeyCode::Enter);

    // Regression: cursor must reset to 0 in the freshly entered directory,
    // not carry over the index (1) from the parent listing.
    assert_eq!(app.left.cursor, 0);
    assert_eq!(app.left.entries.len(), 1);
    assert_eq!(app.left.entries[0].name, "inner.txt");
}

#[test]
fn page_down_and_page_up_move_by_a_full_screen() {
    let dir = tempdir().unwrap();
    let root = vfs_path(dir.path());
    for i in 0..20 {
        fs::write(dir.path().join(format!("f{i:02}")), b"x").unwrap();
    }

    let mut app = App::new(root);
    assert_eq!(app.left.entries.len(), 20);
    // Simulate what `pfnc-tui::render_panel` sets from the real terminal
    // size each frame — these tests drive `App` directly, with no
    // terminal/rendering involved.
    app.left.viewport_height = 5;

    press(&mut app, KeyCode::PageDown);
    assert_eq!(app.left.cursor, 5);
    press(&mut app, KeyCode::PageDown);
    assert_eq!(app.left.cursor, 10);

    press(&mut app, KeyCode::PageUp);
    assert_eq!(app.left.cursor, 5);

    // Clamps at the ends rather than wrapping or erroring.
    for _ in 0..10 {
        press(&mut app, KeyCode::PageUp);
    }
    assert_eq!(app.left.cursor, 0);
    for _ in 0..10 {
        press(&mut app, KeyCode::PageDown);
    }
    assert_eq!(app.left.cursor, 19);
}

#[test]
fn ascend_restores_cursor_to_the_directory_just_left() {
    let dir = tempdir().unwrap();
    let root = vfs_path(dir.path());
    fs::create_dir(dir.path().join("a")).unwrap();
    fs::create_dir(dir.path().join("b")).unwrap();
    fs::create_dir(dir.path().join("c")).unwrap();

    let mut app = App::new(root);
    // Entries sorted: a(0), b(1), c(2).
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Down);
    assert_eq!(app.left.cursor, 2);
    press(&mut app, KeyCode::Enter); // descend into "c"

    press(&mut app, KeyCode::Backspace); // ascend back up
    assert_eq!(
        app.left.entries[app.left.cursor].name, "c",
        "Midnight-Commander behavior: cursor should land back on the folder just left, not reset to the top"
    );
}

#[test]
fn mkdir_via_text_input_creates_directory_and_reloads() {
    let dir = tempdir().unwrap();
    let root = vfs_path(dir.path());
    let mut app = App::new(root.clone());

    press(&mut app, KeyCode::F(7));
    assert!(matches!(app.mode, Mode::TextInput(_)));
    for c in "newdir".chars() {
        press(&mut app, KeyCode::Char(c));
    }
    press(&mut app, KeyCode::Enter);

    assert!(matches!(app.mode, Mode::Browsing));
    assert!(root.join("newdir").as_std_path().is_dir());
    assert!(app.left.entries.iter().any(|e| e.name == "newdir"));
}

#[test]
fn copy_selected_files_to_other_panel_via_f5() {
    let dir = tempdir().unwrap();
    let root = vfs_path(dir.path());
    fs::create_dir(dir.path().join("src")).unwrap();
    fs::write(dir.path().join("src/a.txt"), b"aaa").unwrap();
    fs::write(dir.path().join("src/b.txt"), b"bbb").unwrap();
    fs::create_dir(dir.path().join("dest")).unwrap();

    let mut app = App::new(root.clone());
    // Right panel -> dest (entries sorted: dest(0), src(1); already at 0).
    press(&mut app, KeyCode::Tab);
    press(&mut app, KeyCode::Enter);
    // Left panel -> src, select both files.
    press(&mut app, KeyCode::Tab);
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Enter);
    press(&mut app, KeyCode::Char(' '));
    press(&mut app, KeyCode::Char(' '));

    press(&mut app, KeyCode::F(5));
    assert!(matches!(app.mode, Mode::TextInput(_)));
    press(&mut app, KeyCode::Enter); // confirm prefilled "dest" destination

    assert!(matches!(app.mode, Mode::Progress(_)));
    wait_for_job_to_finish(&mut app);

    assert_eq!(fs::read(root.join("dest/a.txt")).unwrap(), b"aaa");
    assert_eq!(fs::read(root.join("dest/b.txt")).unwrap(), b"bbb");
    // Copy leaves the originals in place.
    assert!(root.join("src/a.txt").as_std_path().exists());
}

#[test]
fn rename_single_item_via_f6() {
    let dir = tempdir().unwrap();
    let root = vfs_path(dir.path());
    fs::write(dir.path().join("old.txt"), b"data").unwrap();

    let mut app = App::new(root.clone());
    press(&mut app, KeyCode::F(6));
    let Mode::TextInput(prompt) = &app.mode else {
        panic!("expected TextInput mode");
    };
    assert_eq!(prompt.text.value, root.join("old.txt").to_string());

    // Clear the prefilled value and type the new full path.
    for _ in 0..prompt.text.value.chars().count() {
        press(&mut app, KeyCode::Backspace);
    }
    for c in root.join("new.txt").to_string().chars() {
        press(&mut app, KeyCode::Char(c));
    }
    press(&mut app, KeyCode::Enter);

    wait_for_job_to_finish(&mut app);
    assert!(!root.join("old.txt").as_std_path().exists());
    assert_eq!(fs::read(root.join("new.txt")).unwrap(), b"data");
}

#[test]
fn delete_with_confirmation_via_f8() {
    let dir = tempdir().unwrap();
    let root = vfs_path(dir.path());
    fs::write(dir.path().join("gone.txt"), b"x").unwrap();

    let mut app = App::new(root.clone());
    press(&mut app, KeyCode::F(8));
    assert!(matches!(app.mode, Mode::Confirm(_)));
    press(&mut app, KeyCode::Char('y'));

    wait_for_job_to_finish(&mut app);
    assert!(!root.join("gone.txt").as_std_path().exists());
}

#[test]
fn confirm_delete_false_skips_dialog_and_deletes_immediately() {
    let dir = tempdir().unwrap();
    let root = vfs_path(dir.path());
    fs::write(dir.path().join("gone.txt"), b"x").unwrap();

    let mut config = Config::default();
    config.general.confirm_delete = false;
    let mut app = App::new_with_config(config, root.clone());

    press(&mut app, KeyCode::F(8));
    assert!(matches!(app.mode, Mode::Progress(_)), "should skip straight to the delete job");

    wait_for_job_to_finish(&mut app);
    assert!(!root.join("gone.txt").as_std_path().exists());
}

#[test]
fn delete_confirmation_cancel_leaves_file_untouched() {
    let dir = tempdir().unwrap();
    let root = vfs_path(dir.path());
    fs::write(dir.path().join("stays.txt"), b"x").unwrap();

    let mut app = App::new(root.clone());
    press(&mut app, KeyCode::F(8));
    press(&mut app, KeyCode::Char('n'));

    assert!(matches!(app.mode, Mode::Browsing));
    assert!(root.join("stays.txt").as_std_path().exists());
}

#[test]
fn sync_via_f4_scans_confirms_and_copies_only_whats_needed() {
    let dir = tempdir().unwrap();
    let root = vfs_path(dir.path());
    fs::create_dir(dir.path().join("dstdir")).unwrap();
    fs::create_dir(dir.path().join("srcdir")).unwrap();
    fs::write(dir.path().join("srcdir/new.txt"), b"fresh").unwrap();
    fs::write(dir.path().join("srcdir/same.txt"), b"identical").unwrap();
    fs::write(dir.path().join("dstdir/same.txt"), b"identical").unwrap();
    fs::write(dir.path().join("dstdir/extra.txt"), b"only on dest").unwrap();

    let mut app = App::new(root.clone());
    // Entries sort alphabetically: dstdir(0), srcdir(1).
    // Right panel -> dstdir (destination).
    press(&mut app, KeyCode::Tab);
    press(&mut app, KeyCode::Enter);
    // Left panel -> srcdir (source, stays active).
    press(&mut app, KeyCode::Tab);
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Enter);

    press(&mut app, KeyCode::F(4)); // sync's default binding
    assert!(matches!(app.mode, Mode::Progress(_)), "should start a scan job");
    wait_for_job_to_finish(&mut app);

    let Mode::Confirm(dialog) = &app.mode else {
        panic!("expected a Confirm summary after the scan, got {:?}", app.mode);
    };
    assert!(dialog.message.contains("1 to copy"), "only new.txt should need copying: {}", dialog.message);
    // extra.txt is dest-only but delete_extraneous defaults to false.
    assert!(!dialog.message.contains("delete"), "must not offer to delete without delete_extraneous: {}", dialog.message);

    press(&mut app, KeyCode::Enter); // confirm
    assert!(matches!(app.mode, Mode::Progress(_)), "should start the execute job");
    wait_for_job_to_finish(&mut app);

    assert_eq!(fs::read(root.join("dstdir/new.txt")).unwrap(), b"fresh");
    assert_eq!(fs::read(root.join("dstdir/same.txt")).unwrap(), b"identical");
    assert!(root.join("dstdir/extra.txt").as_std_path().exists(), "extraneous file must survive a non-mirroring sync");
}

#[test]
fn sync_with_delete_extraneous_removes_dest_only_files() {
    let dir = tempdir().unwrap();
    let root = vfs_path(dir.path());
    fs::create_dir(dir.path().join("dstdir")).unwrap();
    fs::create_dir(dir.path().join("srcdir")).unwrap();
    fs::write(dir.path().join("dstdir/extra.txt"), b"only on dest").unwrap();

    let mut config = Config::default();
    config.general.sync_delete_extraneous = true;
    let mut app = App::new_with_config(config, root.clone());

    press(&mut app, KeyCode::Tab);
    press(&mut app, KeyCode::Enter);
    press(&mut app, KeyCode::Tab);
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Enter);

    press(&mut app, KeyCode::F(4));
    wait_for_job_to_finish(&mut app);

    let Mode::Confirm(dialog) = &app.mode else {
        panic!("expected a Confirm summary after the scan, got {:?}", app.mode);
    };
    assert!(dialog.message.contains("1 to delete"), "{}", dialog.message);

    press(&mut app, KeyCode::Enter);
    wait_for_job_to_finish(&mut app);

    assert!(!root.join("dstdir/extra.txt").as_std_path().exists());
}

#[test]
fn sync_with_nothing_to_do_shows_status_not_a_confirm_dialog() {
    let dir = tempdir().unwrap();
    let root = vfs_path(dir.path());
    fs::create_dir(dir.path().join("dstdir")).unwrap();
    fs::create_dir(dir.path().join("srcdir")).unwrap();
    fs::write(dir.path().join("srcdir/same.txt"), b"identical").unwrap();
    fs::write(dir.path().join("dstdir/same.txt"), b"identical").unwrap();

    let mut app = App::new(root);
    press(&mut app, KeyCode::Tab);
    press(&mut app, KeyCode::Enter);
    press(&mut app, KeyCode::Tab);
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Enter);

    press(&mut app, KeyCode::F(4));
    wait_for_job_to_finish(&mut app);

    assert!(matches!(app.mode, Mode::Browsing), "a no-op sync should not show a confirm dialog");
    assert_eq!(app.status.as_deref(), Some("Nothing to sync"));
}

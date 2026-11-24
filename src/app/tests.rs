
use super::{
    persist::{autosave_path_for, same_cells},
    *,
};

/// App over a one-code-cell notebook, plus its render cache.
pub(super) fn one_cell_app(tag: &str) -> (App, Rendered) {
    let path = std::env::temp_dir().join(format!("jotter-{tag}-{}.ipynb", std::process::id()));
    let nb = serde_json::json!({
        "cells": [{"cell_type": "code", "id": "c1", "metadata": {}, "execution_count": null,
                   "outputs": [], "source": ["x"]}],
        "metadata": {}, "nbformat": 4, "nbformat_minor": 5
    });
    std::fs::write(&path, nb.to_string()).unwrap();
    let (tx, _rx) = mpsc::unbounded_channel();
    let app = App::open(path.clone(), None, tx).unwrap();
    std::fs::remove_file(&path).ok();
    let rendered = Rendered::build(&app.notebook, None, Default::default());
    (app, rendered)
}

pub(super) fn stream(text: &str) -> Value {
    serde_json::json!({"output_type": "stream", "name": "stdout", "text": text})
}

pub(super) fn labels(app: &App) -> Vec<&str> {
    app.completion.as_ref().unwrap().items.iter().map(|c| c.label()).collect()
}

pub(super) fn sources(app: &App) -> Vec<&str> {
    app.notebook.cells.iter().map(|c| c.source.as_str()).collect()
}

pub(super) fn key(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
}

#[test]
fn runs_during_startup_are_queued_and_dropped_on_launch_failure() {
    let (mut app, mut r) = one_cell_app("defer");
    app.kernel_starting = true;
    app.launch = 3;
    app.run_cell(0, &mut r);
    app.run_cell(0, &mut r);
    assert_eq!(app.deferred, ["c1"], "queued once, by cell id");
    // a stale launch's failure is ignored
    app.apply_kernel_event(Event::LaunchFailed { launch: 2, reason: "old".into() }, &mut r);
    assert_eq!(app.deferred.len(), 1);
    app.apply_kernel_event(Event::LaunchFailed { launch: 3, reason: "boom".into() }, &mut r);
    assert!(app.deferred.is_empty() && !app.kernel_starting);
    assert!(app.message.as_deref().unwrap().contains("1 queued run(s) dropped"));
}

#[tokio::test] // the pick spawns a launch
async fn missing_kernel_asks_which_one_to_start_instead() {
    let (mut app, mut r) = one_cell_app("pick");
    app.notebook.extra.insert(
        "metadata".into(),
        serde_json::json!({"kernelspec": {"name": "julia-1.11", "language": "julia"}}),
    );
    app.kernel_name = "julia-1.11".into();
    app.launch = 1;
    let not_found = || Event::NotFound {
        launch: 1,
        wanted: "julia-1.11".into(),
        kernels: vec![
            ("python3".into(), "python".into()),
            ("julia-1.12".into(), "julia".into()),
            ("julia-1.10".into(), "Julia".into()),
        ],
    };
    app.apply_kernel_event(not_found(), &mut r);
    let shown = |app: &App| -> Vec<String> {
        let pick = app.kernel_pick.as_ref().unwrap();
        pick.shown().into_iter().cloned().collect()
    };
    assert_eq!(shown(&app), ["julia-1.12", "julia-1.10"], "only the notebook's language");
    app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &mut r);
    assert!(app.kernel_pick.is_none() && !app.kernel_starting, "Esc: no kernel");
    assert_eq!(app.kernel_name, "julia-1.11");

    app.apply_kernel_event(not_found(), &mut r);
    for c in "A-1.10".chars() {
        app.on_key(key(c), &mut r);
    }
    assert_eq!(shown(&app), ["julia-1.10"], "typing filters, ignoring case");
    app.on_key(key('x'), &mut r);
    app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &mut r);
    assert!(app.kernel_pick.is_some(), "Enter with no match changes nothing");
    app.on_key(KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE), &mut r);
    app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &mut r);
    assert_eq!(app.kernel_name, "julia-1.10");
    assert!(app.kernel_pick.is_none());

    // nothing for the notebook's language: everything is offered
    app.launch = 2;
    app.apply_kernel_event(
        Event::NotFound {
            launch: 2,
            wanted: "x".into(),
            kernels: vec![("rust".into(), "rust".into()), ("ir".into(), "R".into())],
        },
        &mut r,
    );
    assert_eq!(shown(&app), ["rust", "ir"]);
    assert!(app.notebook.extra["metadata"].get("kernelspec").is_none());
}

#[test]
fn restart_asks_first_when_a_kernel_could_be_alive() {
    let (mut app, mut r) = one_cell_app("restart");
    app.kernel_starting = true; // as if a kernel were coming up
    app.on_key(key('R'), &mut r);
    assert_eq!(app.confirm, Some(Confirm::Restart));
    app.on_key(key('x'), &mut r);
    assert!(app.confirm.is_none());
    assert_eq!(app.message.as_deref(), Some("restart cancelled"));
}

#[test]
fn the_kernels_language_info_goes_into_the_notebook() {
    let (mut app, mut r) = one_cell_app("langinfo");
    let info = serde_json::json!({"name": "R", "version": "4.5.1", "file_extension": ".r"});
    app.apply_kernel_event(Event::LanguageInfo(info.clone()), &mut r);
    assert_eq!(app.notebook.extra["metadata"]["language_info"], info);
    assert_eq!(crate::ui::notebook_language(&app.notebook), "r");
}

#[test]
fn update_display_data_replaces_matching_outputs_everywhere() {
    let (mut app, mut r) = one_cell_app("upd");
    let disp = |v: &str| {
        serde_json::json!({"output_type": "display_data",
            "data": {"text/plain": v}, "metadata": {}, "transient": {"display_id": "d1"}})
    };
    app.notebook.cells[0].push_output(disp("old"));
    app.notebook.cells.push(app.notebook.cells[0].clone());
    r.rebuild_all(&app.notebook);
    app.apply_kernel_event(
        Event::UpdateDisplay { display_id: "d1".into(), output: disp("new") },
        &mut r,
    );
    for c in &app.notebook.cells {
        assert_eq!(c.outputs.as_ref().unwrap()[0]["data"]["text/plain"], "new");
    }
    assert!(app.dirty);
}

#[test]
fn input_request_closes_overlays_so_keys_reach_the_box() {
    let (mut app, mut r) = one_cell_app("input-overlay");
    app.help = Some(Help::default());
    app.logs = Some(0);
    let request = jupyter_protocol::messaging::JupyterMessage::new(
        jupyter_protocol::messaging::InputRequest::default(),
        None,
    );
    app.apply_kernel_event(
        Event::Input { request: Box::new(request), prompt: "name: ".into(), password: false },
        &mut r,
    );
    assert!(app.help.is_none() && app.logs.is_none());
    app.on_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE), &mut r);
    assert_eq!(app.stdin_req.as_ref().unwrap().buf, "a");
}

#[test]
fn run_all_follows_the_running_cell_until_manual_movement() {
    let (mut app, mut r) = one_cell_app("follow");
    for _ in 0..2 {
        app.notebook.cells.push(app.notebook.cells[0].clone());
    }
    r.rebuild_all(&app.notebook);
    let start = |app: &mut App, r: &mut Rendered, id: &str, idx: usize| {
        app.running.insert(id.into(), idx);
        app.apply_kernel_event(Event::ExecutionCount { parent: id.into(), count: 1 }, r);
    };
    app.follow = true; // as run_range(0..3) sets it
    start(&mut app, &mut r, "m1", 1);
    assert_eq!(app.selected, 1, "selection jumps to the running cell");
    app.on_key(key('k'), &mut r); // manual movement
    assert!(!app.follow);
    start(&mut app, &mut r, "m2", 2);
    assert_eq!(app.selected, 0, "no more jumping");
    // non-movement keys keep following
    app.follow = true;
    app.on_key(key('o'), &mut r);
    assert!(app.follow);
}

#[test]
fn space_runs_x_runs_all_ctrl_r_redoes() {
    let (mut app, mut r) = one_cell_app("keys");
    app.on_key(key(' '), &mut r);
    assert_eq!(app.message.as_deref(), Some("no kernel — R to (re)start one"));
    app.on_key(key('a'), &mut r);
    app.on_key(key('u'), &mut r);
    assert_eq!(app.notebook.cells.len(), 1);
    app.on_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL), &mut r);
    assert_eq!(app.notebook.cells.len(), 2, "Ctrl+r redoes");
    app.on_key(key('X'), &mut r);
    assert!(app.follow, "run all follows");
}

#[test]
fn missing_path_opens_a_new_notebook_and_recovery_restores() {
    let path = std::env::temp_dir().join(format!("jotter-new-{}.ipynb", std::process::id()));
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::open(path.clone(), None, tx).unwrap();
    assert!(!path.exists(), "nothing written until w");
    assert_eq!(sources(&app), [""]);
    assert!(app.message.as_deref().unwrap().starts_with("new notebook"));
    let mut r = Rendered::build(&app.notebook, None, Default::default());
    // recovery prompt: r swaps in the autosaved notebook as a dirty buffer
    let mut saved = app.notebook.clone();
    saved.cells[0].source = "unsaved work".into();
    assert!(!same_cells(&saved, &app.notebook));
    app.confirm = Some(Confirm::Recover(Box::new(saved), "x".into()));
    app.on_key(key('r'), &mut r);
    assert_eq!(sources(&app), ["unsaved work"]);
    assert!(app.dirty);
}

/// A `.py` notebook: outputs live in memory only. They never dirty the
/// buffer (there is nothing to save), the sidecar still keeps them for a
/// crash, `w` writes percent without them, and `:w x.ipynb` has them.
#[test]
fn py_notebooks_keep_outputs_in_memory_only() {
    let dir = std::env::temp_dir().join(format!("jotter-py-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("nb.py");
    std::fs::write(&path, "# %%\nx = 1\n\n# %% [markdown]\n# hi\n").unwrap();
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::open(path.clone(), None, tx).unwrap();
    let mut r = Rendered::build(&app.notebook, None, Default::default());
    assert_eq!(sources(&app), ["x = 1", "hi"]);
    assert!(!app.stores_outputs());
    app.running.insert("m".into(), 0);
    app.apply_kernel_event(Event::Output { parent: "m".into(), output: stream("out\n") }, &mut r);
    app.apply_kernel_event(Event::ExecutionCount { parent: "m".into(), count: 3 }, &mut r);
    assert!(!app.dirty, "outputs can't be saved to a .py");
    assert!(app.autosave_pending, "but the sidecar keeps them");
    app.maybe_autosave();
    let side = Notebook::read_ipynb(&app.autosave_path()).unwrap();
    assert_eq!(side.cells[0].outputs.as_ref().unwrap().len(), 1);
    // a clean quit drops the sidecar instead of offering the outputs back
    app.quit();
    assert!(app.should_quit && !app.autosave_path().exists());
    // clearing outputs isn't a change to the file either; a source edit is
    app.clear_outputs(0..1, &mut r);
    assert!(!app.dirty);
    app.undo(&mut r);
    assert!(!app.dirty);
    app.update_cell_source("x = 2".into(), &mut r);
    assert!(app.dirty);
    assert!(app.save_notebook(false, &mut r));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "# %%\nx = 2\n\n# %% [markdown]\n# hi\n");
    // save-as converts: the .ipynb gets the in-memory outputs
    let ipynb = dir.join("nb.ipynb");
    assert!(app.save_as(ipynb.to_str().unwrap(), &mut r));
    let saved = Notebook::read_ipynb(&ipynb).unwrap();
    assert_eq!(saved.cells[0].outputs.as_ref().unwrap().len(), 1);
    assert_eq!(saved.cells[0].execution_count(), Some(3));
    assert!(app.stores_outputs());
    // a new .py is a plain script until it has more than one cell
    let fresh = dir.join("new.py");
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::open(fresh.clone(), None, tx).unwrap();
    let mut r = Rendered::build(&app.notebook, None, Default::default());
    app.update_cell_source("print(1)".into(), &mut r);
    assert!(app.save_notebook(false, &mut r));
    assert_eq!(std::fs::read_to_string(&fresh).unwrap(), "print(1)\n");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn autosave_paths_live_in_the_state_dir_keyed_by_absolute_path() {
    let p = autosave_path_for(std::path::Path::new("/data/nb/a.ipynb"));
    assert!(p.ends_with("jotter/autosave/%data%nb%a.ipynb"), "{}", p.display());
    let long = format!("/{}/a.ipynb", "d".repeat(400));
    let name = autosave_path_for(std::path::Path::new(&long));
    assert!(name.file_name().unwrap().len() <= 200);
}

#[test]
fn execution_ends_on_reply_and_idle_in_either_order() {
    let (mut app, mut r) = one_cell_app("end");
    for (first, second) in [
        (Event::Done { parent: "m".into() }, Event::Status { parent: "m".into(), busy: false }),
        (Event::Status { parent: "m".into(), busy: false }, Event::Done { parent: "m".into() }),
    ] {
        app.notebook.cells[0].outputs = Some(Vec::new());
        app.running.insert("m".into(), 0);
        app.apply_kernel_event(first, &mut r);
        // output racing the reply (separate sockets) still lands
        app.apply_kernel_event(
            Event::Output { parent: "m".into(), output: stream("late\n") },
            &mut r,
        );
        assert_eq!(app.notebook.cells[0].outputs.as_ref().unwrap().len(), 1);
        app.apply_kernel_event(second, &mut r);
        assert!(app.running.is_empty() && app.half_done.is_empty());
    }
    // idle of a request we never sent (kernel_info) is ignored
    app.apply_kernel_event(Event::Status { parent: "other".into(), busy: false }, &mut r);
    assert!(app.half_done.is_empty());
}

#[test]
fn non_code_cells_never_receive_kernel_output() {
    let (mut app, mut r) = one_cell_app("toggle");
    app.running.insert("m".into(), 0);
    app.toggle_type(&mut r); // code -> markdown mid-run
    app.apply_kernel_event(Event::ExecutionCount { parent: "m".into(), count: 3 }, &mut r);
    app.apply_kernel_event(Event::Output { parent: "m".into(), output: stream("x\n") }, &mut r);
    let cell = &app.notebook.cells[0];
    assert!(cell.outputs.is_none());
    assert!(!cell.extra.contains_key("execution_count"));
}

#[test]
fn wheel_gesture_latches_to_its_initial_target() {
    let path = std::env::temp_dir().join(format!("jotter-wheel-{}.ipynb", std::process::id()));
    let text: String = (0..40).map(|i| format!("l{i}")).collect::<Vec<_>>().join("\n");
    let nb = serde_json::json!({
        "cells": [{"cell_type": "code", "metadata": {}, "execution_count": null,
                   "source": ["x"],
                   "outputs": [{"output_type": "stream", "name": "stdout", "text": text}]}],
        "metadata": {}, "nbformat": 4, "nbformat_minor": 5
    });
    std::fs::write(&path, nb.to_string()).unwrap();
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::open(path.clone(), None, tx).unwrap();
    std::fs::remove_file(&path).ok();
    let mut rendered = Rendered::build(&app.notebook, None, Default::default());
    // fake a draw: row 0 is the prompt, rows 1.. are the output viewport
    rendered.relayout(80, &app.notebook);
    app.body = Rect::new(0, 0, 80, 30);
    app.content_lines = 100;
    app.hit = (0..30)
        .map(|r| Hit { cell: 0, kind: if r == 0 { HitKind::Other } else { HitKind::Output } })
        .collect();
    assert!(rendered.blocks[0].output_scrollable());
    let wheel = |row: u16, up: bool| MouseEvent {
        kind: if up { MouseEventKind::ScrollUp } else { MouseEventKind::ScrollDown },
        column: 5,
        row,
        modifiers: KeyModifiers::NONE,
    };

    // gesture starts over the prompt -> view; output sliding under the
    // pointer mid-gesture must NOT capture it
    app.on_mouse(wheel(0, false), &mut rendered);
    let after_first = app.scroll;
    assert!(after_first > 0, "view scrolled");
    let out_pos = rendered.blocks[0].win_start();
    app.on_mouse(wheel(5, false), &mut rendered); // now over the output
    assert!(app.scroll > after_first, "gesture stays with the view");
    assert_eq!(rendered.blocks[0].win_start(), out_pos, "output untouched");

    // after a pause, a gesture starting over the output scrolls the output
    app.wheel_latch = None; // simulate the pause
    let view_pos = app.scroll;
    app.on_mouse(wheel(5, true), &mut rendered);
    assert!(rendered.blocks[0].win_start() < out_pos, "output scrolled");
    assert_eq!(app.scroll, view_pos, "view untouched");
}

/// `:w` in the cell editor commits the cell and saves the file; after an
/// external change it refuses with the `:w!` hint, and `:w!` overwrites.
#[test]
fn write_from_editor_saves_and_respects_disk_changes() {
    let path = std::env::temp_dir().join(format!("jotter-w-{}.ipynb", std::process::id()));
    let nb = serde_json::json!({
        "cells": [{"cell_type": "code", "metadata": {}, "execution_count": null,
                   "outputs": [], "source": ["a = 1"]}],
        "metadata": {}, "nbformat": 4, "nbformat_minor": 5
    });
    std::fs::write(&path, nb.to_string()).unwrap();
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::open(path.clone(), None, tx).unwrap();
    let mut rendered = Rendered::build(&app.notebook, None, Default::default());
    let on_disk = || Notebook::open(&path).unwrap().cells[0].source.clone();

    app.write_from_editor("a = 2".into(), false, &mut rendered);
    assert_eq!(on_disk(), "a = 2");
    assert!(!app.dirty);

    std::thread::sleep(std::time::Duration::from_millis(20)); // distinct mtime
    std::fs::write(&path, nb.to_string()).unwrap(); // someone else saved "a = 1"
    app.write_from_editor("a = 3".into(), false, &mut rendered);
    assert_eq!(on_disk(), "a = 1", "refused");
    assert!(app.message.as_deref().unwrap().contains(":w!"));
    assert_eq!(app.notebook.cells[0].source, "a = 3", "edit kept in the notebook");

    app.write_from_editor("a = 3".into(), true, &mut rendered);
    assert_eq!(on_disk(), "a = 3");
    std::fs::remove_file(&path).ok();
}

#[test]
fn external_change_reloads_when_clean_and_prompts_when_dirty() {
    let path = std::env::temp_dir().join(format!("jotter-reload-{}.ipynb", std::process::id()));
    let nb = |src: &str| {
        serde_json::json!({
            "cells": [{"cell_type": "code", "metadata": {}, "execution_count": null,
                       "outputs": [], "source": [src]}],
            "metadata": {}, "nbformat": 4, "nbformat_minor": 5
        })
        .to_string()
    };
    std::fs::write(&path, nb("a = 1")).unwrap();
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::open(path.clone(), None, tx).unwrap();
    let mut rendered = Rendered::build(&app.notebook, None, Default::default());

    // clean buffer: silent reload (nvim autoread)
    std::thread::sleep(std::time::Duration::from_millis(20)); // distinct mtime
    std::fs::write(&path, nb("a = 2")).unwrap();
    app.check_disk(&mut rendered);
    assert_eq!(app.notebook.cells[0].source, "a = 2");
    assert!(app.confirm.is_none());

    // dirty buffer: prompt instead of clobbering local state
    app.notebook.cells[0].source = "local".into();
    app.touch();
    std::thread::sleep(std::time::Duration::from_millis(20));
    std::fs::write(&path, nb("a = 3")).unwrap();
    app.check_disk(&mut rendered);
    assert_eq!(app.confirm, Some(Confirm::Reload));
    assert_eq!(app.notebook.cells[0].source, "local");

    // "keep mine" (any key but l) accepts divergence: no re-prompt, w saves
    let key = KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE);
    app.on_key(key, &mut rendered);
    assert!(app.confirm.is_none());
    app.check_disk(&mut rendered);
    assert!(app.confirm.is_none(), "mtime accepted, no re-prompt");
    assert!(app.save_notebook(false, &mut rendered), "plain w overwrites after keep-mine");
    std::fs::remove_file(&path).ok();
}

/// A paste lands in the cell being edited and nowhere else.
#[test]
fn paste_goes_into_the_editor_only() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::open(PathBuf::new(), None, tx).unwrap();
    let mut r = Rendered::build(&app.notebook, None, Default::default());
    let before = app.notebook.cells.len();
    app.on_paste("arst\n".into(), &mut r); // would insert and retype cells as keys
    assert_eq!(app.notebook.cells.len(), before);
    assert!(app.message.as_deref().unwrap_or("").contains("paste ignored"));

    app.on_key(KeyEvent::new(KeyCode::Char('A'), KeyModifiers::NONE), &mut r);
    app.on_paste("x = 1\r\ny = 2".into(), &mut r);
    let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
    app.on_key(esc, &mut r);
    app.on_key(esc, &mut r);
    assert!(app.editor.is_none());
    assert_eq!(app.notebook.cells[app.selected].source, "x = 1\ny = 2");
}

/// `jotter` without a file: w has nowhere to write, `:w name` names it.
#[test]
fn scratchpad_saves_only_under_a_name() {
    let (tx, _rx) = mpsc::unbounded_channel();
    let mut app = App::open(PathBuf::new(), None, tx).unwrap();
    let mut r = Rendered::build(&app.notebook, None, Default::default());
    assert!(app.is_scratch());
    assert!(!app.save_notebook(false, &mut r));
    let path = std::env::temp_dir().join(format!("jotter-scratch-{}.ipynb", std::process::id()));
    app.ex_command(&format!("w {}", path.display()), &mut r);
    assert_eq!(app.path, path, "{:?}", app.message);
    assert!(!app.dirty && path.exists());
    std::fs::remove_file(&path).ok();
}

#[test]
fn command_line_saves_leaves_and_jumps() {
    let (mut app, mut r) = one_cell_app("cmdline");
    let path = app.path.clone();
    app.disk_mtime = None; // the helper deleted the file: not a disk change
    let type_cmd = |app: &mut App, r: &mut Rendered, cmd: &str| {
        app.on_key(key(':'), r);
        for c in cmd.chars() {
            app.on_key(key(c), r);
        }
        app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), r);
    };
    // builtin editor: `:` opens jOtter's command line; :w commits + saves
    app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &mut r);
    app.on_key(key('A'), &mut r);
    app.on_key(key('y'), &mut r);
    app.on_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &mut r);
    type_cmd(&mut app, &mut r, "w");
    assert!(app.editor.is_some(), ":w keeps editing");
    assert!(!app.dirty, "{:?}", app.message);
    assert!(std::fs::read_to_string(&path).unwrap().contains("xy"));
    type_cmd(&mut app, &mut r, "q");
    assert!(app.editor.is_none(), ":q leaves the cell");
    assert!(!app.should_quit);
    type_cmd(&mut app, &mut r, "bogus");
    assert!(app.message.as_deref().unwrap().contains("not a command"));
    type_cmd(&mut app, &mut r, "q");
    assert!(app.should_quit, ":q in notebook mode quits");
    std::fs::remove_file(&path).ok();
}

#[test]
fn ctrl_k_splits_and_the_old_keys_say_they_are_deprecated() {
    let ctrl = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL);
    for (c, deprecated) in [('k', false), ('\\', true), ('4', true)] {
        let (mut app, mut r) = one_cell_app("split");
        app.on_key(key('i'), &mut r);
        app.on_key(key('x'), &mut r);
        app.on_key(ctrl(c), &mut r);
        assert_eq!(app.notebook.cells.len(), 2, "Ctrl+{c} splits");
        let said = app.message.as_deref().is_some_and(|m| m.contains("goes away"));
        assert_eq!(said, deprecated, "Ctrl+{c}");
    }
}

#[test]
fn the_deprecated_split_keys_are_gone_by_0_8_0() {
    // fails on the 0.8.0 version bump: remove Ctrl+\\ and Ctrl+4 from
    // `on_key_edit`, their docs note, and this test
    let minor: u32 = env!("CARGO_PKG_VERSION_MINOR").parse().unwrap();
    assert!(env!("CARGO_PKG_VERSION_MAJOR") == "0" && minor < 8);
}

#[test]
fn gg_g_and_jump_list() {
    let (mut app, mut r) = one_cell_app("jumps");
    for _ in 0..4 {
        app.on_key(key('a'), &mut r); // 5 cells, selection follows the new one
    }
    app.selected = 2;
    let ctrl = |c| KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL);
    app.on_key(key('g'), &mut r);
    assert_eq!(app.selected, 2, "a lone g waits for its pair");
    app.on_key(key('g'), &mut r);
    assert_eq!(app.selected, 0);
    app.on_key(key('G'), &mut r);
    assert_eq!(app.selected, 4);
    app.on_key(ctrl('o'), &mut r);
    assert_eq!(app.selected, 0);
    app.on_key(ctrl('o'), &mut r);
    assert_eq!(app.selected, 2);
    app.on_key(ctrl('o'), &mut r);
    assert_eq!(app.selected, 2, "oldest entry: stays");
    app.on_key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), &mut r);
    assert_eq!(app.selected, 0, "Tab is Ctrl+i");
    app.on_key(ctrl('i'), &mut r);
    assert_eq!(app.selected, 4);
    // the list follows cells: delete cell 0 and it is skipped
    app.selected = 0;
    app.on_key(key('d'), &mut r);
    app.on_key(key('d'), &mut r);
    app.on_key(key('3'), &mut r);
    app.on_key(key('G'), &mut r); // from the (old) cell 1, now at 0
    assert_eq!(app.selected, 2);
    app.on_key(ctrl('o'), &mut r);
    assert_eq!(app.selected, 0);
}

#[test]
fn visual_selection_acts_on_every_cell() {
    let (mut app, mut r) = one_cell_app("visual");
    for i in 1..5 {
        app.on_key(key('a'), &mut r);
        app.notebook.cells[i].source = format!("c{i}");
    }
    app.notebook.cells[0].source = "c0".into();
    let srcs = |app: &App| sources(app).join(",");
    app.selected = 1;
    for c in ['V', 'j', 'j'] {
        app.on_key(key(c), &mut r);
    }
    assert_eq!(app.sel_range(), 1..4);
    app.on_key(key('K'), &mut r); // block up, stays selected
    assert_eq!(srcs(&app), "c1,c2,c3,c0,c4");
    assert_eq!(app.sel_range(), 0..3);
    app.on_key(key('d'), &mut r);
    assert_eq!(srcs(&app), "c0,c4");
    assert!(app.visual.is_none(), "an operator ends the selection");
    app.on_key(key('p'), &mut r);
    assert_eq!(srcs(&app), "c0,c1,c2,c3,c4");
    assert_eq!(app.selected, 3, "on the last pasted cell");
    app.on_key(key('u'), &mut r);
    app.selected = 0;
    app.on_key(key('P'), &mut r); // before c0
    assert_eq!(srcs(&app), "c1,c2,c3,c0,c4");
    app.on_key(key('u'), &mut r);
    app.on_key(key('p'), &mut r);
    app.on_key(key('u'), &mut r);
    app.on_key(key('u'), &mut r);
    app.on_key(key('u'), &mut r);
    assert_eq!(srcs(&app), "c0,c1,c2,c3,c4", "one undo step each");
    app.selected = 3;
    for c in ['V', 'k', 'k', 'M'] {
        app.on_key(key(c), &mut r);
    }
    assert_eq!(srcs(&app), "c0,c1\nc2\nc3,c4");
    // any other key leaves visual mode without acting
    for c in ['V', 'j', 'o'] {
        app.on_key(key(c), &mut r);
    }
    assert!(app.visual.is_none());
}

#[test]
fn marks_set_jump_and_follow_their_cell() {
    let (mut app, mut r) = one_cell_app("marks");
    for _ in 0..3 {
        app.on_key(key('a'), &mut r);
    }
    app.selected = 1;
    app.on_key(key('m'), &mut r);
    app.on_key(key('a'), &mut r);
    assert_eq!(app.notebook.cells.len(), 4, "ma sets a mark, not a new cell");
    app.selected = 3;
    app.on_key(key('\''), &mut r);
    app.on_key(key('a'), &mut r);
    assert_eq!(app.selected, 1);
    app.on_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL), &mut r);
    assert_eq!(app.selected, 3, "a mark jump is a jump");
    // the mark follows its cell; deleting the cell drops it, undo restores
    app.selected = 0;
    app.on_key(key('J'), &mut r);
    assert_eq!(app.mark_cell('a'), Some(0));
    app.selected = 0;
    for c in ['d', 'd'] {
        app.on_key(key(c), &mut r);
    }
    assert_eq!(app.mark_cell('a'), None);
    app.on_key(key('u'), &mut r);
    assert_eq!(app.mark_cell('a'), Some(0));
    app.on_key(key('`'), &mut r);
    app.on_key(key('b'), &mut r);
    assert!(app.message.as_deref().unwrap().contains("not set"));
    // t (not m) cycles the cell type now
    app.on_key(key('t'), &mut r);
    assert_eq!(app.notebook.cells[app.selected].cell_type, "markdown");
    app.on_key(key(':'), &mut r);
    for c in "delm a".chars() {
        app.on_key(key(c), &mut r);
    }
    app.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &mut r);
    assert!(app.marks.is_empty());
}
